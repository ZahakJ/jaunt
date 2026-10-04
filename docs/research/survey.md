# Prior-art survey for jaunt

Research input for the lead. Written 2026-10-04 by a research agent. Not the design.

Labels used throughout:

- **[V]**: verified today, either by a command I ran on this machine or by reading a primary
  source (URL given).
- **[U]**: unverified. It comes from memory or a secondary source, so treat it as likely but
  check it before relying on it in the README.

The use case under study: one person, about 4 machines (an always-on Arch desktop and about
3 laptops of unknown OS). It needs no accounts, no third-party control plane, a short
setup, sessions that survive drops, and keystroke latency that feels local.

---

## 0. Findings that sharpen or challenge the hypothesis

1. **iroh is now stable.** [V] crates.io API today: `iroh` 1.3.0 and `iroh-relay` 1.3.0 were
   both updated on 2026-09-28. 1.0 shipped 2026-06-15 with a wire-protocol stability promise.
   Since v0.96 its QUIC stack has been **noq**, n0's own fork of quinn, which adds multipath,
   QUIC Address Discovery (QAD) and QUIC NAT traversal
   (https://www.iroh.computer/blog/noq-announcement). The consequence is that the transport
   is **not plain quinn**: it is a young fork that n0 maintains alone. TLS comes from rustls.
2. **No public third-party audit of iroh or noq could be found.** [V] The iroh FAQ
   (https://docs.iroh.computer/about/faq) does not mention one, and neither does the roadmap.
   iroh 1.1.0 fixed a bug that let a malicious relay pin a client at 100% CPU [U, secondary
   source]. "Rests on reviewed primitives" holds for TLS 1.3, rustls and ed25519. It holds
   *less* for the iroh and noq glue, which sits in the security-critical path (it is how a
   peer's key becomes the TLS identity). The README should say this plainly.
3. **The `spake2` crate is explicitly unaudited.** [V] The RustCrypto/PAKEs README
   (https://github.com/RustCrypto/PAKEs) says: "Crates in this repository have not yet
   received any formal cryptographic and security reviews ... USE AT YOUR OWN RISK." The
   SPAKE2 *protocol* is well studied. The Rust *implementation* is small but unreviewed.
   magic-wormhole.rs (0.8.1) uses the same crate. Options are in §5.
4. **iroh's defaults are a third-party control plane.** [V] The docs at
   https://docs.iroh.computer/concepts/relays state that iroh hardcodes a set of public
   relays run by n0. Address lookup (formerly called discovery) also defaults to n0's
   pkarr/DNS servers [U on the exact default set in 1.3]. jaunt must turn both off
   explicitly and supply its own relay URL. If every machine has the **same single
   self-hosted relay** as its home relay, a dialer can reach a peer with only `(endpoint
   id, known relay URL)` and needs no discovery service at all.
5. **The relay needs public reachability with UDP, and the owner has no such host yet.**
   [V, secondary] The relay protocol is WebSocket over HTTPS at `/relay`. QAD, the
   replacement for STUN, is a separate UDP port (default 7842) and needs TLS certificates.
   This machine's sshd listens **only on 127.0.0.1:22** [V, `ss -ltn`], and there is no
   evidence of port forwarding. That leaves three options:
   - Fly.io. It needs a dedicated IPv4 for UDP and a bind to `fly-global-services`, and
     community reports from Aug–Sep 2026 describe UDP replies leaving from a different IP
     and packets not arriving (https://fly.io/docs/networking/udp-and-tcp/,
     community.fly.io threads). Either of those breaks QAD and NAT traversal. **Test this
     before recommending it.**
   - A small VPS.
   - Put the relay's HTTPS/WebSocket side behind the existing Cloudflare tunnel pattern.
     [U, an untested idea] WebSocket would probably pass through, QAD's UDP would not, so
     hole punching would lose its reflexive-address source. Cloudflare would then see relay
     metadata only, never content.
6. **"SSH over iroh" already exists.** [V] `rustonbsd/iroh-ssh` (about 300 stars, v0.2.x,
   https://github.com/rustonbsd/iroh-ssh) dials by endpoint ID and proxies to the local
   sshd. Its README says "anyone with the Endpoint ID can reach your SSH port", meaning it
   has no allowlist. Also: `n0-computer/dumbpipe` 0.39.0 (netcat over iroh, ticket-based),
   `Tryanks/tcode` PR #498 (iroh remote access with single-use 16-byte invitation secrets,
   5-minute lifetime, a lockout after 5 failures, and a self-hosted relay plus pkarr
   server), and `manaflow-ai/cmux`, which moved to an iroh transport. **The idea of
   transport by public key is not novel.** jaunt's value has to come from the pairing,
   the allowlist and revocation, the hop UX, tmux integration and lid-awake behaviour.
7. **SNI leaks identity.** [V, HN thread https://news.ycombinator.com/item?id=48542480]
   The ClientHello carries `<EndpointID>.iroh.invalid` as SNI in clear. An on-path observer
   can therefore recognise and link one of your devices across networks. This is minor for
   this threat model, but it belongs in the threat model.
8. **Cloudflare RTT from here.** [V, measured] `curl -w %{time_connect}` to
   `www.cloudflare.com/cdn-cgi/trace` gave the nearest PoP and a TCP connect time of 35–46 ms
   (5 samples). `ping 1.1.1.1` gave min 13 ms, avg 53 ms, max 143 ms, so the home uplink is
   jittery. With SSH through the tunnel, every keystroke echo goes laptop → PoP →
   Cloudflare backbone → tunnel PoP → cloudflared → sshd and back. **That remains true
   when the laptop sits beside the desktop on the same Wi-Fi.** The expected echo RTT is
   about 2× the PoP RTT (≈ 30–90 ms here, with spikes) against under 2 ms on a direct LAN
   path. [U for the exact figure, since I did not touch the real tunnel.] This is the
   single biggest place where "faster" is real.

---

## 1. Summary table

| Tool | Peers find each other via | NAT traversal / direct rate | Fallback relay | Who controls membership | Root / TUN | Session continuity |
|---|---|---|---|---|---|---|
| WireGuard (bare) | static config: `Endpoint=` IP:port | none; needs at least one reachable endpoint plus `PersistentKeepalive` | none | you (key lists in config) | kernel module or wireguard-go **plus TUN: root/CAP_NET_ADMIN** | roams: the peer endpoint updates from the latest authenticated packet; TCP over it survives IP changes |
| Tailscale | coordination server (SaaS) pushes peer keys and endpoints; DERP for first contact | disco plus STUN-like probing; "well north of 90%" direct [V, tailscale.com/blog/nat-traversal-improvements-pt-1, 2025-10-15] | DERP (Tailscale-run, TCP/HTTPS); **Peer Relays** on your own nodes, free plan gets 2 [V, blog/peer-relays-beta, 2025-10-29] | **the coordination server**, unless Tailnet Lock | TUN by default (root). `--tun=userspace-networking` runs rootless as a SOCKS5/HTTP proxy [V, kb/1112] | stable 100.x IPs, so SSH TCP connections survive roaming; tmux for the rest |
| Headscale | your own coordination server (Go, one binary) | same clients, same traversal | embedded DERP or your own | you (CLI, pre-auth keys, OIDC) | client same as Tailscale; server needs a public HTTPS host | as Tailscale |
| NetBird | management + signal servers (self-hostable, one combined container) | ICE (pion), WireGuard | its own relay (formerly coturn TURN) | management server (accounts / IdP) | kernel WG by default; **rootless netstack mode exists** [V, docs.netbird.io OpenShift page] | stable IPs, like Tailscale |
| Nebula (Slack) | **lighthouses** (your own hosts with static IPs) | UDP hole punching, coordinated by lighthouses | relay nodes (since 1.6) [U on version] | **your own CA** signs host certs; revoke via `pki.blocklist` | TUN (root) | stable overlay IPs |
| ZeroTier | **roots / planet** run by ZeroTier Inc.; you can run your own "moons" or replace the planet | UDP hole punching | roots relay | **network controller** (self-hostable, but under a source-available, non-OSS licence since 1.16) [V, secondary] | TUN (root); libzt for userspace | stable virtual IPs |
| iroh | dial by ed25519 endpoint ID; home relay URL plus address lookup (pkarr/DNS/mDNS) | QUIC hole punching with multipath; "roughly 9 out of 10" direct [V, iroh FAQ] | iroh-relay (HTTPS/WebSocket, self-hostable) | **nobody**: the application decides whom to accept | **none**: userspace UDP socket | QUIC migration and multipath across network changes; the app reconnects after suspend |
| dumbpipe | iroh ticket (endpoint ID + addrs) | iroh | iroh relay | whoever holds the ticket | none | none (a pipe) |
| libp2p (DCUtR) | DHT / rendezvous / relay v2 | DCUtR: **70% ± 7%** conditional success over 4.4 M attempts from 85k networks; TCP ≈ QUIC [V, arXiv 2510.27500] | circuit relay v2 (limited) | app | none | none built in |
| mosh | SSH bootstrap starts `mosh-server` on the remote | **none**: client sends UDP to the server's IP. The server must be reachable on UDP 60000–61000 | none | sshd auth | none (the server runs as the user) | **best in class for roaming and sleep**; SSP state sync; no scrollback |
| Eternal Terminal | SSH bootstrap to `etserver` on TCP 2022 | none | none | sshd auth, plus ET's own key | `etserver` is usually a system service [U]; can run per-user | auto-reconnects TCP; **keeps scrollback**; port forwarding; `et -c "tmux -CC"` |
| SSH + ProxyJump / ControlMaster | DNS / IP; ProxyJump hops through a reachable host | none | the jump host is the relay | `authorized_keys` | sshd needs root to install/configure (already present here) | none; tmux reattach is manual |
| autossh | as SSH | none | none | as SSH | none | restarts dropped SSH (mainly for `-R`/`-L` forwards) |
| magic-wormhole | **mailbox server** with a nameplate number | transit: direct TCP hints, then transit relay | transit relay (TCP) | the code holder (one-shot) | none | n/a (one-shot transfer) |
| cloudflared + Access | public hostname on Cloudflare; the connector dials out to the CF edge | **none needed**: both sides dial out | **always relayed** through Cloudflare | **Cloudflare Access policy** (IdP/SSO/MFA) plus sshd keys | none on the client; connector in Docker | none (plain TCP; drops on network change) |

---

## 2. Notes per system

### WireGuard
- Noise IK and the 1-RTT handshake are formally verified: Tamarin (Donenfeld/Milner),
  CryptoVerif ACCE proof (Lipp), eCK (Dowling/Paterson), ProVerif (Kobeissi/Bhargavan)
  [V, https://www.wireguard.com/formal-verification/]. The kernel implementation has been
  in mainline since 5.6 [U].
- It does no discovery and no NAT traversal. Both sides behind NAT means no tunnel without a
  third party. Every mesh product below exists to add those two things.
- 4-machine setup: generate 4 keypairs, hand-write 4 configs with every peer's key and
  endpoint, and have one reachable hub. That is root on each machine. The ergonomics are
  poor for laptops that roam.
- Security status: the strongest of anything here. The cost is root and manual topology.

### Tailscale
- **Discovery:** the control plane (SaaS) distributes each node's WireGuard key, DERP home
  region and candidate endpoints. Connections start over DERP and upgrade to direct paths
  [V, blog].
- **Direct rate:** "well north of 90%" (Tailscale internal metrics, 2025). DERP throughput
  is far lower. Since 2025-10 Peer Relays let one of your own nodes relay UDP at near-direct
  speed, and the free plan includes 2 [V]. Note that a peer relay needs a reachable UDP port
  on that node.
- **What the coordination server can do:** add a node to your tailnet and push its key to
  every peer, which is a silent MITM or new-member attack. It also sets ACLs and sees
  metadata (which nodes, endpoints, when). **Tailnet Lock** requires new node keys to be
  signed by trusted nodes held by you, "even if Tailscale were malicious or ... hacked".
  Limits: at most 20 signing nodes, not compatible with device approval, and the
  bootstrap is TOFU [V, https://tailscale.com/kb/1226/tailnet-lock]. Plan availability:
  the KB says Personal and Enterprise, and Personal is the free individual plan [U on the
  current naming].
- **Accounts:** login through an identity provider (Google, GitHub, Microsoft, Apple or
  OIDC) is required [U, but long-standing]. That is precisely the third party the owner
  wants out.
- **Rootless:** `tailscaled --tun=userspace-networking` provides SOCKS5/HTTP proxies
  [V, kb/1112]. Outbound SSH works with `ProxyCommand tailscale nc %h %p` [U]. Inbound to
  local services through netstack [U].
- **Free plan:** up to 6 users, unlimited user devices, Tailscale SSH for up to 5 hosts,
  peer relays [V, https://tailscale.com/pricing].
- **4 machines:** install the package (root, or a GUI app on macOS/Windows), run
  `tailscale up`, and log in through the browser on each one. About 10 minutes. Then
  `ssh desktop` works everywhere, with MagicDNS.
- **Continuity:** a stable virtual IP means an SSH TCP connection survives Wi-Fi to hotspot
  changes. A long suspend still kills TCP through keepalive or timeouts, and tmux recovers
  from that.
- **Security review:** a commercial product with public security bulletins; the client is
  open source [U on any recent third-party audit].

### Headscale
- Latest is v0.29.x (v0.29.4, 2026-09-23) [V, secondary, GitHub releases via search].
- Features [V, https://headscale.net/stable/about/features/]: embedded DERP, pre-auth keys,
  OIDC, ACLs, Tailscale SSH, peer relays, MagicDNS, Taildrop. Not supported: Funnel, Serve,
  flow logs. **Tailnet Lock is not listed** and reported as unimplemented [U].
- Who controls what: you do. The Headscale server is now the root of trust: anyone who
  owns it can add nodes, and with no Tailnet Lock there is no second check. It needs a
  publicly reachable HTTPS host, the same hosting problem jaunt's relay has.
- 4 machines: one server (VPS or Fly), `headscale users create me`, a pre-auth key, and
  `tailscale up --login-server=...` on each machine (root). Ongoing operations: upgrades
  that track Tailscale client protocol changes.
- **This is the honest baseline jaunt has to beat:** Headscale with embedded DERP, the
  stock Tailscale clients and tmux gives no accounts, a self-hosted control plane, over 90%
  direct paths and audited WireGuard. jaunt beats it on root, on the TUN/system VPN, on the
  pairing ritual and on hop UX. It does not beat it on maturity.

### NetBird
- Self-hosted stack: management, signal, relay/STUN and dashboard, now available as one
  combined container [V, docs.netbird.io]. Peers use ICE (pion) for traversal over
  WireGuard. It has a rootless netstack mode (userspace WireGuard via gVisor) [V].
- It is built around users, IdP and SSO, so it is account-centric and heavier than this
  use needs. It is attractive for teams.

### Nebula
- You run a **CA** (`nebula-cert`) and **lighthouses** (static-IP hosts that tell peers
  where others are and coordinate hole punching). Relays arrived in 1.6 [U]. Certificates
  carry overlay IP and groups, and a firewall config filters by group.
- Revocation is a `pki.blocklist` of certificate fingerprints pushed to every host. A
  **High** advisory, GHSA-69x3-g4r3-p962 (2026-02-06), allowed a blocklist bypass through
  ECDSA signature malleability [V, https://github.com/slackhq/nebula/security]. This is a
  concrete lesson for jaunt: **revoke by public key, not by certificate or signature
  bytes.**
- Needs TUN (root). The CA ritual is tidy but not short. A lighthouse needs a static IP.

### ZeroTier
- Uses a planet of roots run by ZeroTier Inc. by default; full independence needs your own
  moons or planet. The controller can be self-hosted, but since 1.16 the controller is under
  the ZeroTier Source-Available License, not OSS, and free only for non-commercial use
  [V, secondary: birdhost.de, discuss.zerotier.com]. Uses TUN (root); libzt runs in
  userspace. It has its own crypto protocol (not WireGuard), with less public review than
  WireGuard [U].

### iroh (and dumbpipe)
- An endpoint *is* an ed25519 key. Connections are QUIC with TLS 1.3, and each end
  authenticates with a self-signed cert tied to its endpoint ID. A move to RFC 7250 raw
  public keys is planned [V, FAQ]. Applications get ALPN-multiplexed protocols.
- Relays forward only to endpoint IDs, and only ciphertext. They see "the endpoint IDs and
  IP addresses of the endpoints currently connected to it, plus which endpoints are paired"
  [V, FAQ]. The "9 out of 10" direct rate is n0's own claim [V that they claim it].
- Fully userspace: no root, no TUN. It coexists with any VPN. Multipath means the relay
  path and direct paths live in one connection, so roaming does not require a reconnect.
- **dumbpipe** 0.39.0 [V crates.io] is netcat over iroh. Anyone with the ticket can
  connect, so it has no access control of its own. It is ideal for a 5-minute "does
  direct work between my two laptops?" field test before building anything.
- Gaps for jaunt: no allowlist, pairing, revocation, PTY or session layer. All of that is
  the application's job, which is the part jaunt would write.

### libp2p hole punching
- DCUtR measurement campaign: **70% ± 7.1%** conditional success; when it worked, it worked
  on the first attempt about 97% of the time; TCP and QUIC were indistinguishable
  [V, arXiv 2510.27500]. This is a useful independent number. Mesh-VPN vendor claims of
  over 90% are measured on *their* fleets, many of whose nodes have easy NATs. **For
  planning, assume about 1 in 4–10 of the owner's away-from-home pairings will be relayed**
  (e.g. hotel or CG-NAT hitting a double hard NAT). Relay placement therefore matters for
  latency.
- Too much machinery (DHT, multiaddrs, many transports) for 4 machines.

### mosh
- The client gets the server's address and a session key over SSH, then speaks **SSP** over
  UDP (AES-128-OCB [U]) straight to `mosh-server` on UDP 60000–61000. SSP synchronises screen
  *state*, not a byte stream, so it survives IP changes and sleep without reconnecting.
  **Predictive local echo** underlines speculative characters and hides RTT for typing.
- Costs: **no scrollback** (only the current screen is synchronised, so use tmux). It
  **needs inbound UDP reachability** to the server, which means no NAT traversal and no relay.
  It needs SSH to bootstrap. No stable release since **1.4.0 (2022-10-26)**, though commits
  continued into 2026 [V, secondary: GitHub releases via search]. The design paper is
  USENIX ATC 2012 [U].
- **Combined with Tailscale it works through NAT**, because the UDP rides inside WireGuard.
  "Tailscale + mosh + tmux" is the folk-standard answer to this exact problem [U, but
  widely recommended].
- Lesson for jaunt: the one latency trick that beats physics on a relayed or distant path
  is local echo. It is optional and hard to get right (mosh's prediction engine took years).
  It is out of scope for v1 but worth naming.

### Eternal Terminal
- v7.0.0 (2026-07-07). A 7.0.1 is requested because 7.0.0 fails to build against current
  abseil/protobuf [V, secondary: GitHub issue #882 via search]. TCP on 2022, bootstrapped
  over SSH. It reconnects automatically and keeps native scrollback. It integrates with tmux
  control mode. It does no NAT traversal: it needs a reachable port, or a tunnel under it.
- A useful contrast with mosh: ET chose TCP plus resumable buffers over state sync. jaunt's
  "reconnect and replay unacknowledged bytes" is ET's model, and with tmux underneath even
  replay is mostly unnecessary.

### Plain SSH (ProxyJump, ControlMaster) and autossh
- ProxyJump: `ssh -J desktop laptop2` makes a reachable box the relay. ControlMaster with
  ControlPersist makes a second `ssh host` instant (no handshake). Both are mature,
  audited and present everywhere.
- autossh restarts dead SSH, which suits reverse tunnels. Combined with
  `ssh host -t tmux new -A -s main`, it gives an auto-reattaching session with zero new
  code [U on the current autossh version; not installed here [V `which`]].
- **sshd is the root problem:** a laptop without sshd needs root to install and enable it.
  On macOS, "Remote Login" is an admin toggle [U].

### tmux
- 3.7c on this machine [V `tmux -V`]. It is the session-persistence layer for every option
  above. `tmux new -A -s NAME` attaches or creates. Control mode (`-CC`) exists for
  iTerm2-style integration. OSC 52 carries the clipboard over any pty transport, and rihla
  enables it.

### magic-wormhole (the pairing prior art)
- The code is `<nameplate>-<word>-<word>`, for example `7-guitarist-revenge`. The
  nameplate is a short integer that selects a mailbox on the rendezvous server. The words
  are the SPAKE2 password: **16 bits by default**, so an attacker gets **one guess per
  attempt** (1 in 65536). A failed guess is visible as a "crowded" or "scary" error, and
  codes are single-use [V, https://magic-wormhole.readthedocs.io/en/latest/welcome.html].
- The mailbox server sees only PAKE messages and sealed traffic. A malicious mailbox can MITM
  only by guessing the code once. The transit relay carries ciphertext.
- This is the exact shape jaunt's pairing wants. The rendezvous could be the jaunt relay
  host, or a small ALPN on the always-on desktop.

### cloudflared with Access for SSH (the current setup, described generically)
- **Shape** (generic; the details of the owner's setup are kept out of this repository):
  - A desktop runs a dashboard-managed, token-authenticated `cloudflared` tunnel whose
    ingress points a hostname at the local sshd.
  - sshd listens only on loopback, so the tunnel is the way in from outside.
  - Clients use `ProxyCommand cloudflared access ssh --hostname %h`.
  - Whether an Access *policy* (SSO/MFA) guards the hostname decides how strong such a
    setup really is; it is the first thing to check.
- **Traffic flow:** ssh client → `cloudflared access ssh` (WebSocket/HTTPS to the nearest
  PoP) → Cloudflare network → the PoP holding the tunnel's outbound connection (QUIC or
  HTTP/2) → cloudflared on the desktop → 127.0.0.1:22. **Always relayed, even on the same
  LAN.**
- **What Cloudflare sees and can do in this mode:**
  - Timing, sizes, both endpoints' IPs, the hostname and the identity of whoever logged in
    to Access.
  - Not the SSH plaintext. SSH is end-to-end, and a pinned host key in `known_hosts`
    blocks a Cloudflare MITM [U on the docs wording; follows from the protocol].
  - It can deny service, and it can let *anyone* reach the sshd port if a policy is wrong
    or the account is compromised. sshd key auth still stands behind that.
- **Contrast, other modes:** "Access for Infrastructure" (short-lived SSH certs, **command
  logging**, HPKE-encrypted to your key) needs the Cloudflare One/WARP client and has
  Cloudflare's proxy in the SSH path. The browser-rendered SSH terminal necessarily
  terminates SSH at Cloudflare [V for logging and the WARP requirement,
  https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/use-cases/ssh/ssh-infrastructure-access/;
  U on termination].
- **What it genuinely adds, and jaunt must match or concede each point:**
  1. **No inbound ports anywhere.** jaunt matches this: both sides dial out to the relay.
  2. **A pre-authentication gate.** Unauthenticated internet users cannot even reach sshd,
     so a pre-auth sshd bug is unexploitable. jaunt matches this only if the allowlist is
     enforced at the QUIC/TLS handshake, before any PTY or sshd is touched.
  3. **SSO/MFA and the IdP's account recovery.** A stolen laptop with an unlocked SSH key
     still needs the IdP login, if the policy demands it. **jaunt cannot match this without
     accounts.** The honest equivalent is an optional passphrase-wrapped key, or a per-hop
     PIN or confirmation on the host. Concede this point.
  4. **Central audit logs** of who connected when. jaunt can keep a local log on each host,
     but it is not tamper-evident off-box.
  5. **DDoS shielding.** Irrelevant to jaunt's P2P path. The self-hosted relay is a new,
     small DDoS and abuse target.
  6. **Instant revocation in one dashboard.** jaunt needs revocation to propagate to every
     machine, including ones that are offline at the time. This is the hard part.

---

## 3. Root and userspace matrix for a 4-machine personal fleet

| | needs root on each machine? | changes system networking? | coexists with a corporate/other VPN? | third party with membership power |
|---|---|---|---|---|
| Tailscale (default) | yes (TUN, service, DNS) | yes (routes, MagicDNS) | often, sometimes conflicts [U] | Tailscale (mitigated by Tailnet Lock) plus the IdP |
| Tailscale userspace | no | no (proxy only) | yes | as above |
| Headscale | client yes; server needs a host | yes | as Tailscale | you |
| NetBird | default yes; netstack no | yes / no | as above | your management server and IdP |
| Nebula / ZeroTier | yes | yes | as above | your CA / controller (ZeroTier roots by default) |
| mosh, ET | no (but they need sshd plus reachability) | no | yes | none |
| cloudflared Access | no (client); the connector needs Docker or a service | no | yes | **Cloudflare** plus the IdP |
| **iroh-based jaunt** | **no** | **no** | **yes** | **none** (the relay operator sees metadata only) |

---

## 4. Where "faster, more ergonomic, as secure" is actually achievable, and where it is not

**Clearly achievable:**
- **Latency at home and nearby.** Today, LAN to LAN goes out to a Cloudflare PoP and back
  (≈ 30–90 ms echo, measured jitter up to about 140 ms on ICMP). Any direct-path tool,
  whether Tailscale or jaunt, gives under 2 ms on a LAN and the true internet RTT when away.
  This is the headline win, and **it is not unique to jaunt.**
- **Setup ritual.** Today it is Cloudflare dashboard plus ProxyCommand plus browser login per
  machine. A wormhole-style `jaunt pair` with a code on both screens is genuinely shorter
  than either Cloudflare or Headscale, and it needs no IdP.
- **Survives roaming and drops** without SSH timeouts: QUIC multipath plus tmux. Tailscale
  plus SSH gets most of this. mosh gets more (sleep and local echo).
- **Hop UX:** `jaunt` lists live peers with direct or relay status and RTT, jumps in one
  keystroke, attaches tmux `-A`, and does onward hops without agent-forwarding gymnastics.
  No existing tool does this. It is mostly a thin UX layer, and it could even sit on top of
  Tailscale.

**Not achievable, or not better than Tailscale/Headscale + mosh/tmux:**
- **Direct-path rate and relay quality.** Tailscale has years of traversal tuning,
  port-mapping (UPnP/NAT-PMP/PCP) [U], a global DERP fleet and peer relays. iroh is good
  and its maintainers claim about 90%. jaunt adds nothing here.
- **Audited transport.** WireGuard is formally verified. iroh/noq has no public audit.
  jaunt's transport is *less* reviewed than Tailscale's, though built from reviewed
  primitives.
- **Typing over a bad link.** Only mosh's predictive echo hides RTT. Neither jaunt v1 nor
  Tailscale has it.
- **Account-backed MFA and recovery.** This is the Cloudflare setup's real advantage.
- **Platform reach:** iOS/Android, Windows and macOS GUI clients, mature launchd/systemd
  integration. Tailscale wins outright.

**The strongest argument against building anything:** the owner's actual pain is latency
plus setup. About 15 minutes of `pacman -S tailscale mosh` plus `tailscale up` on 4 machines
(free plan, Tailnet Lock on, keep Cloudflare as a break-glass path) removes the latency
completely and keeps an audited, formally verified data plane maintained by a company whose
job it is. It also comes with phone clients. jaunt would be a new front door to every
machine, written and maintained by one person and AI agents, with an unaudited
QUIC/hole-punching fork and an unaudited PAKE crate in its trust path. Every line of
pairing and revocation code is new attack surface that Tailscale has already had
pen-tested.

**What a rootless, account-less, userspace tool genuinely offers that they do not:**
1. **No third party with membership power, and no IdP.** Tailscale without Tailnet Lock
   lets the SaaS add nodes. Headscale moves that power to a server you must harden. jaunt's
   relay can *never* add a peer, because membership is a local allowlist of keys on each
   machine.
2. **No root and no system networking changes.** It installs like rihla (one `curl | sh`
   into `~/.local`). It works on a machine where the owner lacks admin or does not want a
   system VPN, and it never fights another VPN, DNS or routes.
3. **Purpose-built pairing and revocation** that a human enjoys, with machine-to-machine
   trust that does not depend on any server staying honest.
4. **Hop-native UX with tmux** (list, jump, onward, back) and the lid-awake control, which no
   mesh VPN offers.
5. **A small, self-hosted relay** that sees only ciphertext and endpoint IDs, and can be
   replaced at will.

**Recommendation to the lead:** build jaunt as a **thin, layered** tool, not a new security
core:
- iroh for transport, with n0 relays and discovery turned off.
- An allowlist enforced at accept time, before any byte reaches a PTY.
- **Prefer carrying SSH over the iroh stream** (a ProxyCommand to the local sshd on loopback,
  as iroh-ssh does) wherever sshd exists. The owner then gets two independent, audited
  authentication layers (the iroh key allowlist, then sshd key auth), mirroring the
  Cloudflare setup's "gate plus SSH" defence in depth. Keep a built-in PTY mode only for
  machines with no sshd. That mode is a single-layer design and should be documented as
  such.
- Say plainly in the README that Tailscale plus mosh plus tmux is the conservative
  alternative.

---

## 5. Pairing: options for the PAKE problem

| Option | Speakable | Rests on reviewed code | Notes |
|---|---|---|---|
| SPAKE2 via `spake2` crate (wormhole style) | yes (16–45 bits) | protocol yes; **crate no** [V] | Same crate as magic-wormhole.rs. Small; could be pinned and fuzzed. Name it as a residual risk. |
| CPace / OPAQUE | yes | no mature audited Rust crate known [U] | Not better in practice. |
| **SAS comparison** (Bluetooth numeric comparison / ZRTP / Signal safety-number style). The code is only a rendezvous name; both sides exchange endpoint IDs, connect over iroh TLS, and both screens show words derived from `H(idA‖idB‖transcript)` for the human to confirm. | yes (the code), plus a compare step | **yes**: TLS 1.3, rustls, a hash | Needs both screens, which the owner has: they are all their own machines. No PAKE at all. MITM requires the attacker to win the rendezvous *and* the human to approve mismatched words. |
| High-entropy one-time token (Tailscale auth key / tcode invite style, 128 bits) | no (paste or QR) | yes | Most robust; least fun. A good fallback (`jaunt pair --token`). |

The SAS variant, or SPAKE2 *plus* SAS, matches "do not design new crypto" best. SAS comparison
is an established pattern, and it removes the unaudited crate from the trust root.

---

## 6. What to take from the owner's own prior art

**leyline (pairing and threat-model taste)** [V, read README.md and docs/SECURITY.md]:
- Speakable code `NN-word-word-word` (100 × 7776³ ≈ 45.4 bits, CSPRNG with rejection
  sampling). It is designed to "survive a phone call". It can also be shared as a link (the
  code in the URL **fragment**, wiped after reading) or as a **QR**. jaunt could print a QR
  in the terminal so a laptop can pair from a phone photo, but keep the words primary.
- **"The infrastructure is untrusted by construction."** The server sees only an opaque
  `roomId = HKDF(rootKey, "room-id")`, never the code. Signalling is sealed under a key
  derived from the code. jaunt's relay or rendezvous should likewise learn nothing that
  lets it join, MITM or enumerate.
- Single-use codes, about 20-minute expiry, **rooms lock after the first pairing** (a leaked
  code races the real party detectably), per-IP rate limits on unknown-room joins, a key
  confirmation before any data flows, and a **short auth string shown on both UIs** for
  humans to compare. The owner already likes the SAS step.
- The threat model is written as two tables: **Defended against** (adversary → why they
  fail) and **Explicitly NOT defended against** (metadata, a leaked code, the served code,
  DoS). It names the one secret stored at rest and how it is bounded (TTL, deletion,
  scope). jaunt's SECURITY.md should copy this structure, including the comparison with
  the Cloudflare setup and an honest "where Cloudflare is stronger" list (§2).
- Release gates exercise the **unhappy paths**: kill the signalling server mid-transfer,
  restart it mid-pairing, and a blindness audit of every relayed frame. jaunt's harness
  should likewise kill the relay mid-session (the direct path must survive) and assert that
  the relay sees no plaintext.

**rihla (tmux and install taste)** [V, read README.md]:
- tmux prefix **`Ctrl-Space`** (`Ctrl-b` kept as a second prefix). `prefix |` and `prefix -`
  split. **`Ctrl-h/j/k/l`** move between panes and Neovim splits, so **jaunt must not take
  those keys**. `prefix h` opens a menu and `prefix H` a cheat sheet, and the OSC 52
  clipboard works over SSH. User overrides live in `~/.config/rihla/tmux/local.conf`.
  rihla's `~/.tmux.conf` sources rihla's own file.
  - Integration idea: jaunt ships an *optional* tmux snippet (e.g. `prefix J` opens a
    `display-menu` or popup of peers) that the user adds to their local.conf. jaunt never
    edits `~/.tmux.conf` itself.
  - Nested tmux, meaning a local tmux containing a remote tmux, needs a rule. Options are a
    different remote prefix, or `prefix` pressed twice to send it through. Decide
    explicitly.
- Install style:
  - One `curl -fsSL .../install.sh | sh`, plus a wget fallback and a "read it first"
    variant.
  - **No root.** Everything in `~/.local/{bin,share,state}` and `~/.config/<tool>`.
  - **Idempotent** ("running it again changes nothing and says so").
  - Pinned and sha256-checked downloads.
  - One marked PATH block in shell rc files, removed byte for byte on uninstall.
  - A journal-based `uninstall`.
  - A `doctor` command that says what is missing and what that costs.
  - A "first five minutes" section, and a **Tested on** matrix that honestly separates
    container-tested from hand-tested (macOS was "CI only, nobody used it by hand").
  - Decisions recorded in a "Decisions" section.
  - jaunt should copy all of this, including the `doctor` command and the honest matrix.
- rihla deliberately left out tmux session persistence plugins. That fits jaunt using plain
  `tmux new -A -s <name>` and not resurrect-style plugins.

---

## 7. Open questions for the lead
1. Is there an Access policy (SSO/MFA) on the SSH hostname today? That decides how much
   "as secure" jaunt must match (§2, points 3 and 6).
2. Where will the relay run with working UDP for QAD? Fly's UDP quirks need a test. A VPS is
   safer. Running WebSocket only behind Cloudflare would degrade hole punching.
3. sshd on the laptops: if it is absent (macOS needs the Remote Login admin toggle), the
   SSH-over-iroh layering needs root once per laptop. Is a built-in PTY server acceptable as
   a single-layer fallback?
4. Revocation propagation to offline machines: push an allowlist delta signed by any
   remaining member at next contact, and have the revoked machine's key rejected
   immediately wherever the owner runs `jaunt revoke`. Learn from Nebula's advisory: match
   on raw public key bytes only.
5. Run a quick `dumbpipe` field test between two real networks before committing, to
   measure the direct-path rate on the owner's actual networks.

## Sources (fetched or queried 2026-10-04)
- crates.io API: iroh, iroh-relay (1.3.0), dumbpipe (0.39.0), spake2 (0.4.0), magic-wormhole (0.8.1)
- https://docs.iroh.computer/about/faq · https://docs.iroh.computer/concepts/relays · https://www.iroh.computer/roadmap · https://www.iroh.computer/blog/noq-announcement · https://news.ycombinator.com/item?id=48542480
- https://github.com/RustCrypto/PAKEs · https://github.com/rustonbsd/iroh-ssh · https://github.com/n0-computer/dumbpipe · https://github.com/Tryanks/tcode/pull/498
- https://tailscale.com/blog/nat-traversal-improvements-pt-1 · https://tailscale.com/blog/peer-relays-beta · https://tailscale.com/kb/1226/tailnet-lock · https://tailscale.com/kb/1112/userspace-networking · https://tailscale.com/pricing
- https://headscale.net/stable/about/features/ · https://github.com/juanfont/headscale/releases
- https://docs.netbird.io/get-started/install/openshift · https://github.com/slackhq/nebula/security · https://birdhost.de/en/blog/netbird-vs-zerotier
- https://www.wireguard.com/formal-verification/ · https://arxiv.org/pdf/2510.27500
- https://magic-wormhole.readthedocs.io/en/latest/welcome.html
- https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/use-cases/ssh/ (and its ssh-infrastructure-access and ssh-cloudflared-authentication subpages)
- https://fly.io/docs/networking/udp-and-tcp/ and community.fly.io UDP threads (Aug–Sep 2026)
- Local commands (read-only): `ss -ltn`, `tmux -V`, `cloudflared --version`, `which ...`, curl/ping RTT to Cloudflare
